#![no_std]

use ipc::{Message, port};
use spin::Mutex;

// ── Protocol helper ──────────────────────────────────────────────────────────

fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off + 8].try_into().unwrap_or([0u8; 8]))
}

fn make_reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

fn err_reply(e: i32) -> Message { make_reply(e as i64) }
fn val_reply(v: u64) -> Message { make_reply(v as i64) }

// ── Linux input_event ────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct input_event {
    pub time: timeval,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

const ZERO_EVENT: input_event = input_event {
    time: timeval { tv_sec: 0, tv_usec: 0 },
    type_: 0, code: 0, value: 0,
};

// EV_SYN=0, EV_KEY=1, EV_ABS=3 (linux/input-event-codes.h).
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
/// SYN_REPORT: the frame terminator every evdev consumer batches on.
const SYN_REPORT: u16 = 0;
/// SYN_DROPPED: "your queue overflowed, your state is stale, resync".
const SYN_DROPPED: u16 = 3;

// ── Device / client state ────────────────────────────────────────────────────

const MAX_DEVICES: usize = 4;

// Device classes we surface. Keyboard = event0 (13:64), tablet = event1 (13:65,
// absolute pointer: ABS_X/ABS_Y + BTN_LEFT, NO INPUT_PROP_DIRECT so libinput
// classifies it as a pointer, not a touchscreen).
const DEV_KEYBOARD: usize = 0;
const DEV_TABLET: usize = 1;

// A tablet emits X + Y + SYN (and buttons) per motion frame; a fast drag bursts
// well past 64, so one client's queue is 256 deep to hold a whole gesture even
// for a reader that only wakes at 60 Hz.
const CLIENT_EVENTS: usize = 256;

/// Live client queues, pooled across every node rather than fixed per device:
/// a compositor holds keyboard *and* tablet, and during a VT/session handover
/// the outgoing and incoming sessions hold both at once. 16 covers that with
/// headroom; the pool is ~96 KiB of BSS (every field zero-initialised, so it
/// costs nothing in the image — see TODO.md item 15).
pub const MAX_CLIENTS: usize = 16;

/// `open_id` marking the in-kernel console drain's queue. The VFS allocates
/// real open ids from 1 upward out of a 1024-entry table, so `u32::MAX` can
/// never collide with one.
pub const CONSOLE_OPEN_ID: u32 = u32::MAX;

/// One open file description's event queue.
///
/// Linux gives every `open("/dev/input/eventN")` its own queue and broadcasts
/// each event to all of them. We used to have a single ring per *device*, so
/// two readers of one node robbed each other: the `[EVSTAT]` census measured
/// `dev=0 push=128 conspop=112 deliv=16` — the in-kernel console drain took 112
/// of 128 keystrokes and the userspace client got the other 16. That is
/// cosmetic while only one consumer matters and fatal once two do, which is
/// exactly what a greeter handing over to a session compositor is.
struct EvClient {
    in_use: bool,
    dev_id: u32,
    /// VFS per-open cookie (`VnodeKind::DynamicDevice::open_id`), or
    /// `CONSOLE_OPEN_ID` for the console tap.
    ///
    /// 0 means the request carried no cookie and the queue is keyed by `pid`
    /// alone. The VFS forwards `open_id` in message slot 4 for ioctls only;
    /// until it does the same for VFS_READ/VFS_POLL (see the integration note
    /// on `client_key`), a reader is identified by its process, which is
    /// per-*process* rather than per-*open* but already separates the
    /// consumers that were robbing each other.
    open_id: u32,
    pid: u32,
    /// VT this queue was registered on (1-based, as [`tty_server::vt::active`]
    /// reports it), or **0 for a queue that follows the active VT** rather than
    /// being pinned to one.
    ///
    /// A compositor opens its input devices while it is foreground, so the VT
    /// that was active at registration is the VT it belongs to; `broadcast`
    /// then skips it while some other VT is on screen. The console tap keeps 0
    /// by definition — the in-kernel line discipline *is* whichever VT is
    /// active, so it can never be off-screen.
    ///
    /// The failure mode this tag has is worth naming: a device opened while
    /// backgrounded gets tagged with the background VT and goes silent until
    /// that VT comes back. That is why registration logs the tag — the symptom
    /// is "input stopped working", and only the tag says why.
    vt: u32,
    events: [input_event; CLIENT_EVENTS],
    head: usize,
    count: usize,
    /// Monotonic push counter for THIS queue — the poll/epoll readiness
    /// sequence (edge emulation). Per client, because an edge is only an edge
    /// for the reader that has not consumed it.
    seq: u64,
    deliv: u64,
    dropped: u64,
    /// Last time this queue was read/polled, for LRU reclamation.
    touched: u64,
}

impl EvClient {
    const fn empty() -> Self {
        Self {
            in_use: false,
            dev_id: 0,
            open_id: 0,
            pid: 0,
            vt: 0,
            events: [const { ZERO_EVENT }; CLIENT_EVENTS],
            head: 0,
            count: 0,
            seq: 0,
            deliv: 0,
            dropped: 0,
            touched: 0,
        }
    }

    fn enqueue(&mut self, ev: input_event) {
        let tail = (self.head + self.count) % CLIENT_EVENTS;
        self.events[tail] = ev;
        self.count += 1;
    }

    fn push(&mut self, ev: input_event) {
        if self.count >= CLIENT_EVENTS {
            // Linux (drivers/input/evdev.c) does not slide the window on
            // overflow: it discards the client's whole queue and makes
            // SYN_DROPPED the next event that client reads. Half a motion
            // frame is worse than an admitted gap — it moves the pointer to a
            // coordinate that was never sent — and an admitted gap is
            // something libinput knows how to resynchronise from.
            self.dropped += self.count as u64;
            self.head = 0;
            self.count = 0;
            self.enqueue(input_event { time: ev.time, type_: EV_SYN, code: SYN_DROPPED, value: 0 });
        }
        self.enqueue(ev);
        self.seq = self.seq.wrapping_add(1);
    }

    /// Discard everything queued and make `SYN_DROPPED` the next event read.
    ///
    /// The same treatment `push` gives an overflow, applied for a different
    /// reason: a queue that was gated off for the length of a VT switch holds
    /// events from before the switch and is missing every event during it, and
    /// both halves of that are stale state a client would otherwise act on —
    /// libinput would replay a key-down whose release it never saw, or a
    /// pointer coordinate from a gesture that ended minutes ago. `SYN_DROPPED`
    /// is precisely the "your state is stale, re-read it from the device"
    /// signal libinput already knows how to resynchronise from, so a resume
    /// costs nothing new on either side.
    fn force_resync(&mut self, now_us: u64) {
        self.dropped += self.count as u64;
        self.head = 0;
        self.count = 0;
        self.enqueue(input_event {
            time: timeval { tv_sec: (now_us / 1_000_000) as i64,
                            tv_usec: (now_us % 1_000_000) as i64 },
            type_: EV_SYN, code: SYN_DROPPED, value: 0,
        });
        self.seq = self.seq.wrapping_add(1);
    }

    fn pop(&mut self) -> Option<input_event> {
        if self.count == 0 { return None; }
        let ev = self.events[self.head];
        self.head = (self.head + 1) % CLIENT_EVENTS;
        self.count -= 1;
        Some(ev)
    }
}

struct EvdevDevice {
    in_use: bool,
    /// CLOCK id events are stamped with (EVIOCSCLOCKID), as the caller last set
    /// it, with 0 standing for the CLOCK_MONOTONIC default. Stored inverted
    /// like that so the whole `STATE` static stays zero-initialised and lands
    /// in .bss: a `clockid: 1` initialiser would drag ~96 KiB of client queues
    /// into .data with it. Advisory either way — we always stamp from the same
    /// monotonic clock `clock_gettime(CLOCK_MONOTONIC)` reports, which is what
    /// libinput asks for.
    clockid_override: u32,
    /// The open holding `EVIOCGRAB` on this node, keyed exactly the way
    /// [`EvdevState::find`] keys a client: by `open_id` when there is one, by
    /// `pid` when there is not. Both zero means no grab, which keeps the whole
    /// `STATE` static in .bss.
    grab_open: u32,
    grab_pid: u32,
    /// The VT the grab was taken on.
    ///
    /// A grab **cannot survive a switch away from that VT**, and this field is
    /// what says which VT that is. It is recorded rather than compared on the
    /// push path because the release is driven by the switch itself
    /// ([`evdev_vt_activated`]) — a grab that merely went *dormant* while its
    /// VT was off screen would come back live when the VT did, and "the client
    /// I switched away from is still holding the keyboard" is precisely the
    /// state this is meant to make unreachable.
    grab_vt: u32,
}

impl EvdevDevice {
    const fn empty() -> Self {
        Self { in_use: false, clockid_override: 0, grab_open: 0, grab_pid: 0, grab_vt: 0 }
    }
    fn grabbed(&self) -> bool { self.grab_open != 0 || self.grab_pid != 0 }
    fn ungrab(&mut self) { self.grab_open = 0; self.grab_pid = 0; self.grab_vt = 0; }
}

/// Does this queue belong to the open `(open_id, pid)` names?
///
/// The one place the client key is spelled out, so `find`, the grab holder and
/// the `VFS_CLOSE` arm cannot drift apart — a grab recorded under one keying
/// and looked up under another is a grab with no holder, i.e. a deaf machine.
fn client_matches(c: &EvClient, open_id: u32, pid: u32) -> bool {
    c.in_use && if open_id != 0 { c.open_id == open_id }
                else { c.open_id == 0 && c.pid == pid }
}

struct EvdevState {
    devs: [EvdevDevice; MAX_DEVICES],
    clients: [EvClient; MAX_CLIENTS],
    /// Monotonic counter stamped into `EvClient::touched`.
    tick: u64,
}

impl EvdevState {
    const fn empty() -> Self {
        Self {
            devs: [const { EvdevDevice::empty() }; MAX_DEVICES],
            clients: [const { EvClient::empty() }; MAX_CLIENTS],
            tick: 0,
        }
    }

    /// Index of the queue `(dev, open_id, pid)` reads from, or None.
    fn find(&self, dev: u32, open_id: u32, pid: u32) -> Option<usize> {
        self.clients.iter().position(|c| c.dev_id == dev && client_matches(c, open_id, pid))
    }

    /// Same, registering a queue if this is the first request from that open.
    fn find_or_register(&mut self, dev: u32, open_id: u32, pid: u32) -> Option<usize> {
        self.tick = self.tick.wrapping_add(1);
        if let Some(i) = self.find(dev, open_id, pid) {
            self.clients[i].touched = self.tick;
            return Some(i);
        }
        let slot = match self.clients.iter().position(|c| !c.in_use) {
            Some(i) => i,
            // Pool exhausted: reclaim the least recently used queue, never the
            // console tap. A client that stopped reading is by construction the
            // oldest, and a pid-keyed queue (see `EvClient::open_id`) gets no
            // VFS_CLOSE naming it, so this is its only reclamation path.
            None => self.clients.iter().enumerate()
                .filter(|(_, c)| c.in_use && c.open_id != CONSOLE_OPEN_ID)
                .min_by_key(|(_, c)| c.touched)
                .map(|(i, _)| i)?,
        };
        let tick = self.tick;
        // The VT this open belongs to, decided ONCE, here. The console tap
        // follows the active VT instead of being pinned to one — see
        // `EvClient::vt`.
        let vt = if open_id == CONSOLE_OPEN_ID { 0 } else { tty_server::vt::active() as u32 };
        let c = &mut self.clients[slot];
        // Reset the bookkeeping only — `events` is 6 KiB and unreachable while
        // `count` is 0, so zeroing it would just be a memset on the open path.
        c.in_use = true;
        c.dev_id = dev;
        c.open_id = open_id;
        c.pid = pid;
        c.vt = vt;
        c.head = 0;
        c.count = 0;
        c.seq = 0;
        c.deliv = 0;
        c.dropped = 0;
        c.touched = tick;
        log_registration(dev, open_id, pid, vt);
        Some(slot)
    }

    /// Deliver one event to every queue open on `dev`. Returns true if any of
    /// them was full and lost its contents.
    ///
    /// `console_ok` false skips the console tap only — see `push_event`.
    ///
    /// `active_vt` is the VT currently on screen, and a queue pinned to a
    /// different one does not receive the event.
    ///
    /// `exempt` bypasses **both** the VT gate and the grab. It is set for
    /// serial frames only — see `push_event`.
    ///
    /// Both filters live HERE, downstream of `push_event`'s `chord_key` and
    /// `escape_key` calls, and that ordering is not incidental: Ctrl+Alt+Fn and
    /// Ctrl+Alt+Esc are consumed before any per-client routing runs, so neither
    /// the gate nor a grab can be put in front of the two key sequences that
    /// escape a VT holding the display. Nothing in this function can make the
    /// machine unrecoverable, because nothing in this function is reached by
    /// the keys that recover it.
    fn broadcast(&mut self, dev: u32, ev: input_event, console_ok: bool,
                 active_vt: u32, exempt: bool) -> bool {
        // Resolve the grab holder, and SELF-HEAL a grab that no longer has
        // one. A grab whose client is gone would silence the node forever, and
        // the whole point of item 20's escape hatch is that no state in here
        // gets to be permanent.
        let grab_idx = if self.devs[dev as usize].grabbed() {
            let (g_open, g_pid) =
                (self.devs[dev as usize].grab_open, self.devs[dev as usize].grab_pid);
            match self.clients.iter()
                      .position(|c| c.dev_id == dev && client_matches(c, g_open, g_pid)) {
                Some(i) => Some(i),
                None => { self.devs[dev as usize].ungrab(); None }
            }
        } else {
            None
        };

        let mut overflowed = false;
        for (i, c) in self.clients.iter_mut().enumerate() {
            if !(c.in_use && c.dev_id == dev) { continue; }
            if c.open_id == CONSOLE_OPEN_ID && !console_ok { continue; }
            if !exempt {
                if active_vt != 0 && c.vt != 0 && c.vt != active_vt { continue; }
                // EVIOCGRAB: exclusive delivery to the holder, as on Linux —
                // the in-kernel console tap included, because a compositor that
                // grabs the keyboard means it, and a console still echoing
                // underneath a grab is the bug the grab was asked to fix.
                if let Some(g) = grab_idx {
                    if i != g { continue; }
                }
            }
            if c.count >= CLIENT_EVENTS { overflowed = true; }
            c.push(ev);
        }
        overflowed
    }

    /// `EVIOCGRAB` — take or release exclusive delivery on `dev`.
    ///
    /// Returns 0, or a negative errno.
    fn set_grab(&mut self, dev: u32, open_id: u32, pid: u32, want: bool) -> i32 {
        let d = &self.devs[dev as usize];
        let (g_open, g_pid) = (d.grab_open, d.grab_pid);
        let held_by_caller = g_open == open_id && g_pid == pid;

        if !want {
            // Only the holder releases, as on Linux (`evdev_ungrab` answers
            // EINVAL to anyone else, including a caller that never grabbed).
            if !d.grabbed() || !held_by_caller { return -22; } // EINVAL
            self.devs[dev as usize].ungrab();
            return 0;
        }

        if d.grabbed() && !held_by_caller {
            // Contested — unless the recorded holder no longer exists, in
            // which case the stale grab loses rather than locking the node out
            // of every future grab.
            let live = self.clients.iter()
                .any(|c| c.dev_id == dev && client_matches(c, g_open, g_pid));
            if live { return -16; } // EBUSY
            self.devs[dev as usize].ungrab();
        }

        // Register the caller's queue up front. A grab whose holder has no
        // queue resolves to nothing in `broadcast`, and `broadcast` would then
        // (correctly) drop the grab on the very first event — a grab that
        // reported success and did not hold is worse than one that failed.
        if self.find_or_register(dev, open_id, pid).is_none() { return -12; } // ENOMEM
        let vt = tty_server::vt::active() as u32;
        let d = &mut self.devs[dev as usize];
        d.grab_open = open_id;
        d.grab_pid = pid;
        d.grab_vt = vt;
        0
    }

    /// Drop every grab on every node. The escape hatch's whole implementation;
    /// see [`release_all_grabs`].
    fn ungrab_all(&mut self) -> bool {
        let mut any = false;
        for d in self.devs.iter_mut() {
            if d.grabbed() { d.ungrab(); any = true; }
        }
        any
    }
}

static STATE: Mutex<EvdevState> = Mutex::new(EvdevState::empty());

// ── Interrupt Safety ─────────────────────────────────────────────────────────

extern "C" {
    fn arch_interrupt_save() -> usize;
    fn arch_interrupt_restore(f: usize);
    /// Monotonic nanoseconds since boot, sub-tick resolution, never decreasing —
    /// the same clock `clock_gettime(CLOCK_MONOTONIC)` reports to userspace.
    fn arch_monotonic_ns() -> u64;
    /// Raw UART byte, the same seam `mm::gap2` writes through. Used only by
    /// `log_registration`, which runs in task context.
    fn arch_serial_putc(c: u8);
}

/// Total events ever handed to `push_event`, across every device. A guest-side
/// witness that host-injected input actually reached the kernel ring: QMP
/// accepting `input-send-event` only proves the host queued it.
static EVENTS_PUSHED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// See `EVENTS_PUSHED`.
pub fn events_pushed() -> u64 {
    EVENTS_PUSHED.load(core::sync::atomic::Ordering::Relaxed)
}

// ── Poll-wake coalescing ─────────────────────────────────────────────────────
//
// `push_event` ends by waking pollers, and `sched::try_wake_poll` is not a
// targeted wake: `POLL_WAIT_CHANNEL` is ONE global channel shared by every task
// parked in poll/epoll_wait anywhere. So each event costs a RUN_QUEUE
// acquisition, an unblock of every polling process in the session, and an IPI —
// and then each woken process re-probes its epoll set, finds nothing for it,
// and re-blocks, taking RUN_QUEUE again on the way down.
//
// That cost is per EVENT, and a frame is several events. Press and release
// shift and nothing renders, but four events go through here — key down,
// SYN_REPORT, key up, SYN_REPORT — so four whole-system herds. Pointer motion
// is three per sample (ABS_X, ABS_Y, SYN_REPORT) at the tablet's report rate.
// The symptom is a desktop that slows down in proportion to how fast you type
// or move, with no relation to what is drawn.
//
// ⚠ A related plan — per-instance wait channels — was REFUTED by measurement
// during the 2026-08-10 sluggishness work: the herd was 0.7% once the epoll
// scans were bounded. That measurement was taken on an IDLE desktop, which is a
// different regime from a sustained input storm, so it does not settle this
// case — but it does mean the modes below exist to be MEASURED against each
// other, not assumed. `WAKE_MODE_BASELINE` reproduces the pre-existing
// behaviour exactly, so the comparison is against the real thing.
//
// Switching mode is a one-line edit plus a kernel rebuild. `[DRMSTAT]` reports
// `pollwake` (wakes actually issued) alongside the existing `evpush` (events
// pushed); their ratio is the whole measurement.

/// Wake on every event — the behaviour before any of this existed. The control.
pub const WAKE_MODE_BASELINE: u8 = 0;
/// Wake only on `SYN_REPORT`. A consumer cannot act on a partial frame —
/// libinput accumulates events and dispatches at the report — so the wakes this
/// drops could never have been acted on. Frees 2× on keys, 3× on motion.
pub const WAKE_MODE_SYN: u8 = 1;
/// Wake once per drain burst. Costs nothing in latency: `poll_events` runs off
/// the 100 Hz tick and drains the whole virtqueue in one loop, so every event in
/// a burst becomes visible to userspace at the same instant either way — one
/// wake after the drain is what the per-event wakes already added up to. Bounds
/// wakes at the tick rate no matter how fast input arrives.
pub const WAKE_MODE_BURST: u8 = 2;

/// Which of the three above is compiled in. Change this line and rebuild the
/// kernel to compare; `[DRMSTAT]`'s `pollwake` field tells you which build a log
/// came from without having to guess.
pub const WAKE_MODE: u8 = WAKE_MODE_BURST;

/// Wakes actually issued, against `EVENTS_PUSHED` wakes attempted. Under
/// `WAKE_MODE_BASELINE` the two climb together; the ratio IS the saving.
static POLL_WAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// See `POLL_WAKES`.
pub fn poll_wakes() -> u64 {
    POLL_WAKES.load(core::sync::atomic::Ordering::Relaxed)
}

/// Set by `push_event` under `WAKE_MODE_BURST`, drained by `flush_pending_wake`.
static WAKE_PENDING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Issue the wake and count it. IRQ-safe: `try_wake_poll` is the non-blocking
/// form and returns false rather than waiting on a contended RUN_QUEUE.
fn wake_pollers() {
    if sched::try_wake_poll() {
        POLL_WAKES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    } else if WAKE_MODE == WAKE_MODE_BURST {
        // Deferred, not lost: leave the flag set so the next drain retries.
        // The other modes already rely on the 100 Hz tick as their backstop.
        WAKE_PENDING.store(true, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Issue one wake if any event was pushed since the last flush.
///
/// Call at the END of an input drain, after every `push_event` that burst will
/// make — the tick hooks in `arch/*/timer.rs` (which drain virtio input AND the
/// UART, in that order), the aarch64 UART IRQ, and the x86_64 PS/2 IRQ. The
/// non-tick callers flush for themselves rather than waiting for the next tick,
/// so coalescing costs them no latency either.
///
/// A no-op in the other modes, and cheap in all of them: one relaxed load.
pub fn flush_pending_wake() {
    if WAKE_MODE != WAKE_MODE_BURST { return; }
    if WAKE_PENDING.swap(false, core::sync::atomic::Ordering::Relaxed) {
        wake_pollers();
    }
}

/// True while the last `EV_KEY` pushed on the keyboard node was a serial
/// synthetic, so the `SYN_REPORT` that follows it can be exempted from the VT
/// gate along with the key itself.
///
/// The UART drain pushes a PAIR — `EV_KEY value=2` then `EV_SYN/SYN_REPORT`
/// (`arch/*/timer.rs`, `arch/aarch64/src/exception.rs`) — and an evdev frame is
/// not a frame without its terminator: libinput accumulates events and acts on
/// them only at `SYN_REPORT`. Exempting the key but gating its report would
/// hand a backgrounded reader keystrokes it can never complete, which is a
/// worse failure than dropping them, because it is silent and unbounded.
///
/// Best effort by construction: a real keyboard IRQ landing between the two
/// halves of a serial frame mis-tags one `SYN_REPORT`. The cost either way is
/// a single stray or missing frame marker, and the interleaving itself is
/// pre-existing — the pair was never pushed atomically.
static LAST_KB_SERIAL: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

// ── Per-device I/O census ────────────────────────────────────────────────────
//
// `EVENTS_PUSHED` proves host-injected input reached the ring. It cannot say
// whether anything in userspace ever DRAINS that ring, and those two failures
// look identical from outside: a compositor that opened `/dev/input/event1` and
// never reads, and one that never opened it at all, both leave the ring filling
// and then overwriting. These counters split them — `reads`/`polls`/`ioctls`
// are nonzero only if some process is actually talking to the node.
//
// Every field is a relaxed atomic so the 0.5 Hz sampler can run from the timer
// IRQ without taking a lock. The one field that needs the state lock is the
// live queue depth; it is sampled with `try_lock` and reports `u64::MAX` when
// the sample was MISSED, never 0 — a missed sample and an empty queue must not
// print the same, which is the whole point of measuring the depth.
macro_rules! ev_counters {
    ($($name:ident),* $(,)?) => {
        $(static $name: [core::sync::atomic::AtomicU64; MAX_DEVICES] =
            [const { core::sync::atomic::AtomicU64::new(0) }; MAX_DEVICES];)*
    };
}
ev_counters!(
    C_PUSHED,   // events handed to push_event for this device
    C_DROPPED,  // pushes that overflowed at least one client's queue
    C_READS,    // VFS_READ calls
    C_EAGAIN,   // VFS_READ calls that found the caller's queue empty
    C_DELIV,    // input_event records actually copied out by read()
    C_POLLS,    // VFS_POLL calls
    C_POLLIN,   // VFS_POLL calls that answered POLLIN
    C_IOCTLS,   // VFS_IOCTL calls
    C_ENOTTY,   // VFS_IOCTL calls answered ENOTTY (unimplemented request)
    C_LASTNR,   // ioctl nr of the most recent ENOTTY, so it can be identified
    C_CONSPOP,  // events consumed by the in-kernel console via pop_event()
    C_RPID,     // pid of the most recent reader
    C_IPID,     // pid of the most recent ioctl caller
);

fn bump(c: &[core::sync::atomic::AtomicU64; MAX_DEVICES], dev: usize) {
    if dev < MAX_DEVICES { c[dev].fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
}
fn setv(c: &[core::sync::atomic::AtomicU64; MAX_DEVICES], dev: usize, v: u64) {
    if dev < MAX_DEVICES { c[dev].store(v, core::sync::atomic::Ordering::Relaxed); }
}

/// One device's I/O census. `depth == u64::MAX` means the state lock was busy
/// and neither `depth` nor `clients` was sampled (see the module note above).
#[derive(Clone, Copy)]
pub struct EvCensus {
    pub pushed:  u64,
    pub dropped: u64,
    /// Deepest client queue on this device — the shared ring's `depth` has no
    /// successor now that every open has its own.
    pub depth:   u64,
    /// Queues currently open on this device, console tap included.
    pub clients: u64,
    pub reads:   u64,
    pub eagain:  u64,
    pub deliv:   u64,
    pub polls:   u64,
    pub pollin:  u64,
    pub ioctls:  u64,
    pub enotty:  u64,
    pub lastnr:  u64,
    pub conspop: u64,
    pub rpid:    u64,
    pub ipid:    u64,
}

/// Sample `dev`'s census. Safe from IRQ context: relaxed atomic loads plus one
/// `try_lock` for the queue depth.
pub fn census(dev: usize) -> EvCensus {
    use core::sync::atomic::Ordering::Relaxed;
    if dev >= MAX_DEVICES {
        return EvCensus { pushed: 0, dropped: 0, depth: 0, clients: 0, reads: 0,
                          eagain: 0, deliv: 0, polls: 0, pollin: 0, ioctls: 0,
                          enotty: 0, lastnr: 0, conspop: 0, rpid: 0, ipid: 0 };
    }
    let (depth, clients) = match STATE.try_lock() {
        Some(st) => {
            let mut deepest = 0u64;
            let mut n = 0u64;
            for c in st.clients.iter() {
                if c.in_use && c.dev_id == dev as u32 {
                    n += 1;
                    if c.count as u64 > deepest { deepest = c.count as u64; }
                }
            }
            (deepest, n)
        }
        None => (u64::MAX, u64::MAX),
    };
    EvCensus {
        pushed:  C_PUSHED[dev].load(Relaxed),
        dropped: C_DROPPED[dev].load(Relaxed),
        depth,
        clients,
        reads:   C_READS[dev].load(Relaxed),
        eagain:  C_EAGAIN[dev].load(Relaxed),
        deliv:   C_DELIV[dev].load(Relaxed),
        polls:   C_POLLS[dev].load(Relaxed),
        pollin:  C_POLLIN[dev].load(Relaxed),
        ioctls:  C_IOCTLS[dev].load(Relaxed),
        enotty:  C_ENOTTY[dev].load(Relaxed),
        lastnr:  C_LASTNR[dev].load(Relaxed),
        conspop: C_CONSPOP[dev].load(Relaxed),
        rpid:    C_RPID[dev].load(Relaxed),
        ipid:    C_IPID[dev].load(Relaxed),
    }
}

/// One client queue's census. Per-device totals cannot show the defect this
/// pool exists to fix — `push=128 deliv=16` is equally consistent with one
/// starved reader and with sixteen well-fed ones — so the numbers that matter
/// are per queue.
#[derive(Clone, Copy)]
pub struct EvClientCensus {
    pub dev_id:  u32,
    /// `CONSOLE_OPEN_ID` for the in-kernel console tap; a VFS per-open cookie
    /// otherwise; 0 for a queue keyed by pid alone (see `EvClient::open_id`).
    pub open_id: u32,
    pub pid:     u32,
    /// The VT this queue is pinned to, or 0 for one that follows the active VT
    /// (see `EvClient::vt`). Printed because "this client's `deliv` is flat"
    /// and "this client is gated off" are the same observation only if the tag
    /// is visible next to the count.
    pub vt:      u32,
    pub queued:  u32,
    pub deliv:   u64,
    pub dropped: u64,
}

impl EvClientCensus {
    pub const fn empty() -> Self {
        Self { dev_id: 0, open_id: 0, pid: 0, vt: 0, queued: 0, deliv: 0, dropped: 0 }
    }
}

/// Fill `out` with one entry per live client queue and return how many were
/// written. Returns `usize::MAX` when the state lock was busy — the sample was
/// MISSED, which must not print the same as "no clients".
///
/// Safe from IRQ context: one `try_lock`, no allocation, no user memory.
pub fn clients_census(out: &mut [EvClientCensus]) -> usize {
    let st = match STATE.try_lock() { Some(s) => s, None => return usize::MAX };
    let mut n = 0;
    for c in st.clients.iter() {
        if !c.in_use { continue; }
        if n >= out.len() { break; }
        out[n] = EvClientCensus {
            dev_id: c.dev_id,
            open_id: c.open_id,
            pid: c.pid,
            vt: c.vt,
            queued: c.count as u32,
            deliv: c.deliv,
            dropped: c.dropped,
        };
        n += 1;
    }
    n
}

// ── evdev capability constants (linux/input-event-codes.h) ────────────────────

const BUS_VIRTUAL: u16 = 0x06;
const ABS_X: usize = 0;
const ABS_Y: usize = 1;

// ── User-copy helpers (all go through the caller's address space) ─────────────

fn copy_out(pid: u32, dst: usize, src: &[u8]) -> Message {
    let n = src.len();
    let srcp = src.as_ptr() as usize;
    let ok = sched::with_task_address_space(pid, || {
        unsafe { core::ptr::copy_nonoverlapping(srcp as *const u8, dst as *mut u8, n); }
        0i32
    });
    match ok { Some(0) => val_reply(n as u64), _ => err_reply(-14) }
}

fn copy_in(pid: u32, src: usize, dst: &mut [u8]) -> Option<()> {
    let n = dst.len();
    let dstp = dst.as_mut_ptr() as usize;
    sched::with_task_address_space(pid, || {
        unsafe { core::ptr::copy_nonoverlapping(src as *const u8, dstp as *mut u8, n); }
        0i32
    }).map(|_| ())
}

fn zero_out(pid: u32, dst: usize, len: usize) -> Message {
    let ok = sched::with_task_address_space(pid, || {
        unsafe { core::ptr::write_bytes(dst as *mut u8, 0, len); }
        0i32
    });
    match ok { Some(0) => val_reply(len as u64), _ => err_reply(-14) }
}

/// EVIOCGBIT(ev, len): report the capability bitmask for event type `ev`.
fn eviocgbit(dev_id: usize, ev: usize, arg_ptr: usize, size: usize, pid: u32) -> Message {
    const MAXB: usize = 96; // covers the KEY bitmap up to ~KEY code 0x2FF
    let mut buf = [0u8; MAXB];
    let n = core::cmp::min(size, MAXB);
    match ev {
        0 => { // supported event types
            if n >= 1 {
                buf[0] = if dev_id == DEV_TABLET { 0x0B } else { 0x03 };
            }
        }
        1 => { // EV_KEY
            if dev_id == DEV_TABLET {
                // BTN_LEFT/RIGHT/MIDDLE = 0x110/0x111/0x112 → byte 34, bits 0..2.
                let byte = 0x110 >> 3;
                if byte < n { buf[byte] = 0x07; }
            } else {
                // keyboard advertises the full key range (as before).
                for b in buf[..n].iter_mut() { *b = 0xFF; }
            }
        }
        3 => { // EV_ABS
            if dev_id == DEV_TABLET && n >= 1 {
                buf[0] = 0x03; // ABS_X | ABS_Y
            }
        }
        _ => {} // EV_REL etc → none
    }
    copy_out(pid, arg_ptr, &buf[..n])
}

/// EVIOCGABS(abs): report input_absinfo for an absolute axis (tablet only).
fn eviocgabs(dev_id: usize, abs: usize, arg_ptr: usize, pid: u32) -> Message {
    // input_absinfo{ value, min, max, fuzz, flat, resolution } = 6×i32 = 24B.
    // Both axes: 0..32767, resolution 0 (libinput rejects X-xor-Y and mismatched
    // resolution).
    if dev_id == DEV_TABLET && (abs == ABS_X || abs == ABS_Y) {
        let info: [i32; 6] = [0, 0, 32767, 0, 0, 0];
        copy_out(pid, arg_ptr, unsafe {
            core::slice::from_raw_parts(info.as_ptr() as *const u8, 24)
        })
    } else {
        zero_out(pid, arg_ptr, 24)
    }
}

// ── Message Dispatch ──────────────────────────────────────────────────────────

/// `(open_id, pid)` — which open a request came from.
///
/// INTEGRATION NOTE. The VFS forwards the per-open cookie in message slot 4 for
/// VFS_IOCTL only (`handle_ioctl`, servers/vfs/src/lib.rs). Until it does the
/// same for VFS_READ and VFS_POLL, slot 4 reads as 0 on those and the queue is
/// keyed by the caller's pid from slot 3 — per-process instead of per-open,
/// which already separates the consumers that were robbing each other but makes
/// two opens *within* one process share a queue. Adding slot 4 to those two
/// proxies is all it takes to get true Linux semantics; nothing here changes.
fn client_key(msg: &Message) -> (u32, u32) {
    (arg(msg, 4) as u32, arg(msg, 3) as u32)
}

pub fn handle(msg: &Message, _caller_pid: u32, _target_port: u32) -> Message {
    let tag = msg.tag;
    let dev_id = arg(msg, 0) as usize;

    if dev_id >= MAX_DEVICES { return err_reply(-19); } // ENODEV

    match tag {
        vfs_server::VFS_READ => {
            let buf_ptr = arg(msg, 1) as usize;
            let count = arg(msg, 2) as usize;
            let (open_id, pid) = client_key(msg);
            bump(&C_READS, dev_id);
            setv(&C_RPID, dev_id, pid as u64);

            let event_size = core::mem::size_of::<input_event>();
            let mut total_copied = 0usize;
            loop {
                // Drain a chunk under the lock, then copy it out WITHOUT it:
                // write_user_buf can take a demand-paging fault, and faulting
                // while holding a lock the timer IRQ's push_event also takes is
                // the 82d0cc3 hazard shape.
                let mut chunk = [ZERO_EVENT; 8];
                let mut n = 0usize;
                {
                    let f = unsafe { arch_interrupt_save() };
                    let mut st = STATE.lock();
                    let slot = match st.find_or_register(dev_id as u32, open_id, pid) {
                        Some(s) => s,
                        None => { drop(st); unsafe { arch_interrupt_restore(f); } break; }
                    };
                    while n < 8 && total_copied + (n + 1) * event_size <= count {
                        match st.clients[slot].pop() {
                            Some(ev) => { chunk[n] = ev; n += 1; }
                            None => break,
                        }
                    }
                    st.clients[slot].deliv += n as u64;
                    drop(st);
                    unsafe { arch_interrupt_restore(f); }
                }
                if n == 0 { break; }

                let bytes = n * event_size;
                let ok = sched::with_current_address_space(|as_| {
                    unsafe {
                        as_.write_user_buf(buf_ptr + total_copied,
                            core::slice::from_raw_parts(chunk.as_ptr() as *const u8, bytes))
                    }
                }).unwrap_or(false);
                if !ok { return err_reply(-14); } // EFAULT
                total_copied += bytes;
            }

            if total_copied == 0 {
                bump(&C_EAGAIN, dev_id);
                return err_reply(-11); // EAGAIN
            }
            C_DELIV[dev_id].fetch_add((total_copied / event_size) as u64,
                                      core::sync::atomic::Ordering::Relaxed);
            val_reply(total_copied as u64)
        }
        vfs_server::VFS_WRITE => {
            let count = arg(msg, 2) as u64;
            val_reply(count)
        }
        vfs_server::VFS_IOCTL => {
            let cmd = arg(msg, 1) as usize;
            let pid = arg(msg, 3) as u32;
            let arg_ptr = arg(msg, 2) as usize;
            let open_id = arg(msg, 4) as u32;

            // ioctl request encoding: dir(2) size(14) type(8) nr(8).
            let nr   = cmd & 0xFF;
            let typ  = (cmd >> 8) & 0xFF;
            let size = (cmd >> 16) & 0x3FFF;

            bump(&C_IOCTLS, dev_id);
            setv(&C_IPID, dev_id, pid as u64);

            if cmd == 0x541B { // FIONREAD (type 'T', not 'E')
                // The caller's OWN queue depth. An ioctl never registers a
                // queue — a probe must not cost one of the 16 slots, and every
                // reader reaches VFS_READ/VFS_POLL anyway.
                let f = unsafe { arch_interrupt_save() };
                let queued = {
                    let st = STATE.lock();
                    st.find(dev_id as u32, open_id, pid).map_or(0, |i| st.clients[i].count)
                };
                unsafe { arch_interrupt_restore(f); }
                let count = (queued * core::mem::size_of::<input_event>()) as i32;
                return copy_out(pid, arg_ptr, &count.to_ne_bytes());
            }
            if typ != 0x45 { // not an 'E' ioctl → ENOTTY
                bump(&C_ENOTTY, dev_id);
                setv(&C_LASTNR, dev_id, cmd as u64);
                return err_reply(-25);
            }

            match nr {
                0x01 => val_reply(0x00010001), // EVIOCGVERSION
                0x02 => { // EVIOCGID → input_id{bustype,vendor,product,version} (8B)
                    let (vendor, product) = if dev_id == DEV_TABLET { (0x0627u16, 0x0001u16) }
                                            else { (0x0627u16, 0x0002u16) };
                    let ids: [u16; 4] = [BUS_VIRTUAL, vendor, product, 0x0001];
                    copy_out(pid, arg_ptr, unsafe {
                        core::slice::from_raw_parts(ids.as_ptr() as *const u8, 8)
                    })
                }
                0x06 => { // EVIOCGNAME(len)
                    let name: &[u8] = if dev_id == DEV_TABLET { b"QEMU Virtio Tablet\0" }
                                      else { b"QEMU Virtio Keyboard\0" };
                    let n = core::cmp::min(size, name.len());
                    copy_out(pid, arg_ptr, &name[..n])
                }
                0x07 | 0x08 => err_reply(-2), // EVIOCGPHYS/UNIQ → ENOENT (empty)
                0x09 => { // EVIOCGPROP → INPUT_PROP_POINTER for the tablet so libinput
                          // classifies it as an absolute POINTER (not a touchscreen)
                          // and delivers BTN_LEFT as a pointer button — required for
                          // click-to-focus (a compositor sets keyboard focus on a
                          // pointer button press). Keyboard advertises no props.
                    if dev_id == DEV_TABLET {
                        let mut buf = [0u8; 8];
                        buf[0] = 1 << 0; // bit 0 = INPUT_PROP_POINTER
                        let n = core::cmp::min(size, buf.len());
                        copy_out(pid, arg_ptr, &buf[..n])
                    } else {
                        zero_out(pid, arg_ptr, size)
                    }
                }
                0x18 | 0x19 | 0x1b => zero_out(pid, arg_ptr, size), // EVIOCGKEY/LED/SW → zeroed
                0xa0 => { // EVIOCSCLOCKID(int) — store; we already stamp monotonic
                    let mut clk = [0u8; 4];
                    if copy_in(pid, arg_ptr, &mut clk).is_none() { return err_reply(-14); }
                    let f = unsafe { arch_interrupt_save() };
                    STATE.lock().devs[dev_id].clockid_override = u32::from_ne_bytes(clk);
                    unsafe { arch_interrupt_restore(f); }
                    val_reply(0)
                }
                0x90 => { // EVIOCGRAB — ENFORCED (TODO.md item 20)
                    // `_IOW('E', 0x90, int)`, but the int is passed BY VALUE:
                    // userspace calls `ioctl(fd, EVIOCGRAB, 1)`, so `arg_ptr`
                    // holds 1 or 0 and is not a pointer to dereference.
                    //
                    // This was accepted-and-ignored until now, and the comment
                    // it replaces gave the right reason for that: a real grab
                    // takes the in-kernel console drain with it, and there was
                    // nothing to switch back to. There is now — and, more to
                    // the point, there are three independent ways out that do
                    // not depend on this code being correct: Ctrl+Alt+Fn and
                    // Ctrl+Alt+Esc are consumed upstream of `broadcast`, a
                    // switch drops every grab (`evdev_vt_activated`), and the
                    // serial line is exempt.
                    let want = arg_ptr != 0;
                    let f = unsafe { arch_interrupt_save() };
                    let rc = STATE.lock().set_grab(dev_id as u32, open_id, pid, want);
                    unsafe { arch_interrupt_restore(f); }
                    if rc == 0 { val_reply(0) } else { err_reply(rc) }
                }
                // EVIOCREVOKE — still accepted and ignored. Revoking an fd is
                // not the grab's mechanism (it permanently poisons the
                // description rather than redirecting delivery) and nothing in
                // this tree issues it; implementing it on the strength of the
                // grab work would be shipping an untested second teardown path
                // for the one subsystem that must not have one.
                0x91 => val_reply(0),
                _ if (0x20..0x40).contains(&nr) => // EVIOCGBIT(ev, len)
                    eviocgbit(dev_id, nr - 0x20, arg_ptr, size, pid),
                _ if (0x40..0x60).contains(&nr) => // EVIOCGABS(abs)
                    eviocgabs(dev_id, nr - 0x40, arg_ptr, pid),
                _ => { // ENOTTY — an 'E' request we do not implement. Recorded
                       // with its nr because libinput/libevdev probe a long
                       // list of them and exactly one fatal gap is enough.
                    bump(&C_ENOTTY, dev_id);
                    setv(&C_LASTNR, dev_id, nr as u64);
                    err_reply(-25)
                }
            }
        }
        vfs_server::VFS_POLL => {
            // POLLIN when THIS open has an event queued (raw evdev fds read
            // whole input_event records, so a pending SYN is readable too), and
            // `seq` is that queue's own push counter for epoll edge emulation.
            // Answering from a shared ring made one reader's drain silently
            // un-ready every other reader.
            let (open_id, pid) = client_key(msg);
            let f = unsafe { arch_interrupt_save() };
            let (count, seq) = {
                let mut st = STATE.lock();
                match st.find_or_register(dev_id as u32, open_id, pid) {
                    Some(i) => (st.clients[i].count, st.clients[i].seq),
                    None => (0, 0),
                }
            };
            unsafe { arch_interrupt_restore(f); }
            let revents: u32 = if count > 0 { 0x1 } else { 0 };
            bump(&C_POLLS, dev_id);
            if revents != 0 { bump(&C_POLLIN, dev_id); }
            let mut m = Message::empty();
            m.data[0..8].copy_from_slice(&(revents as u64).to_le_bytes());
            m.data[8..16].copy_from_slice(&seq.to_le_bytes());
            m
        }
        vfs_server::VFS_CLOSE => {
            // Sent once the LAST fd on this open is gone, node in slot 0 and the
            // open cookie in slot 1 (mirrors the DRM server's arm). Retire the
            // queue so the pool is not held by a process that has exited.
            let open_id = arg(msg, 1) as u32;
            if open_id != 0 && open_id != CONSOLE_OPEN_ID {
                let f = unsafe { arch_interrupt_save() };
                let mut st = STATE.lock();
                if let Some(i) = st.find(dev_id as u32, open_id, 0) {
                    st.clients[i].in_use = false;
                    st.clients[i].count = 0;
                }
                // Drop a grab this open was holding. `broadcast` would
                // self-heal it on the next event anyway, but only once an
                // event arrives — and while the node is grabbed by a corpse
                // there are no events to arrive with. The close is the last
                // moment anything is guaranteed to run.
                let d = &mut st.devs[dev_id];
                if d.grab_open == open_id && d.grabbed() { d.ungrab(); }
                drop(st);
                unsafe { arch_interrupt_restore(f); }
            }
            val_reply(0)
        }
        _ => err_reply(-38), // ENOSYS
    }
}

/// Pop one event for the in-kernel console (`read_input_byte`).
///
/// The console is a client like any other, holding a permanently registered
/// queue (`CONSOLE_OPEN_ID`), rather than sharing a ring with userspace. That
/// is the half of the robbery the census caught from the kernel side:
/// `conspop=112` of `push=128`, with `deliv=16` left for the compositor.
///
/// Its queue is fed only while the keystroke is the line discipline's to have —
/// see the `console_ok` gate in `push_event`.
pub fn pop_event(dev_id: u32) -> Option<input_event> {
    if dev_id as usize >= MAX_DEVICES { return None; }
    let f = unsafe { arch_interrupt_save() };
    let mut st = STATE.lock();
    let ev = st.find(dev_id, CONSOLE_OPEN_ID, 0).and_then(|i| {
        let ev = st.clients[i].pop();
        if ev.is_some() { st.clients[i].deliv += 1; }
        ev
    });
    drop(st);
    unsafe { arch_interrupt_restore(f); }
    if ev.is_some() { bump(&C_CONSPOP, dev_id as usize); }
    ev
}

/// True if the console's queue holds anything at all.
pub fn has_events(dev_id: u32) -> bool {
    if dev_id as usize >= MAX_DEVICES { return false; }
    let f = unsafe { arch_interrupt_save() };
    let st = STATE.lock();
    let n = st.find(dev_id, CONSOLE_OPEN_ID, 0).map_or(0, |i| st.clients[i].count);
    drop(st);
    unsafe { arch_interrupt_restore(f); }
    n > 0
}

/// True if the console's queue holds at least one real key-down/serial event
/// (`type_ == EV_KEY`, `value == 1` or `2`) rather than only SYN markers or
/// key-release events. The kernel's `read_input_byte` silently discards
/// those non-actionable entries by popping and skipping them, so a lone
/// leftover SYN — always pushed right after every key event, including one
/// whose matching key-down byte a single-byte `read()` already consumed —
/// leaves `has_events()` true with nothing left that `read_input_byte`
/// would ever actually return. Used for fd 0's poll/epoll readiness check,
/// which must agree with what a following `read()` can really produce.
pub fn has_key_event(dev_id: u32) -> bool {
    if dev_id as usize >= MAX_DEVICES { return false; }
    let f = unsafe { arch_interrupt_save() };
    let st = STATE.lock();
    let found = match st.find(dev_id, CONSOLE_OPEN_ID, 0) {
        Some(i) => {
            let c = &st.clients[i];
            (0..c.count).any(|k| {
                let ev = &c.events[(c.head + k) % CLIENT_EVENTS];
                ev.type_ == EV_KEY && (ev.value == 1 || ev.value == 2)
            })
        }
        None => false,
    };
    drop(st);
    unsafe { arch_interrupt_restore(f); }
    found
}

pub fn push_event(dev_id: u32, type_: u16, code: u16, value: i32) {
    if dev_id as usize >= MAX_DEVICES { return; }

    // Stamp from the same monotonic clock userspace reads, at its full
    // resolution. libinput asks for CLOCK_MONOTONIC and compares event times
    // against its own clock_gettime(); everything downstream of it measures the
    // interval between events. A whole-tick stamp gave every event drained in
    // one 10 ms tick an identical timeval and a tv_usec that was always a
    // multiple of 10 000. Read it before masking interrupts and before taking
    // the state lock: push_event runs in IRQ context and this is two atomic
    // loads and a counter read — no locks and no user memory — but there is
    // still no reason to hold anything across it.
    let now_us = unsafe { arch_monotonic_ns() } / 1_000;
    EVENTS_PUSHED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    // Ctrl+Alt+Fn is recognised here rather than in a keyboard driver because
    // push_event is the one choke point every keyboard source funnels through,
    // so a future USB HID path is covered without being listed anywhere. It sits
    // ahead of the broadcast, not inside it: the chord has to be consumed for
    // EVERY queue at once, or one open still sees the F-key that moved the
    // display out from under it. Counted as pushed before the return so the
    // `evpush` witness still accounts for every event that reached the kernel;
    // it simply reaches no queue.
    //
    // `chord_key` is safe from this IRQ context: it does nothing but relaxed
    // fetch_or/fetch_and/load on CHORD_MODS and ACTIVE_KB_P1 and, on a hit, one
    // relaxed store in switch_request. No lock, no bounded-at-best loop, no
    // framebuffer call — the actual switch runs later from task context in
    // `vt::poll_deferred`. It also applies the K_XLATE gate itself (a raw-mode
    // VT owner does its own switching, as on Linux), so that rule is NOT
    // repeated here: one copy of it, in the module that owns the mode.
    if dev_id == DEV_KEYBOARD as u32 && type_ == EV_KEY {
        if tty_server::vt::chord_key(code, value) {
            bump(&C_PUSHED, dev_id as usize);
            return;
        }
        // Ctrl+Alt+Esc — the escape hatch (TODO.md item 20). Same position as
        // the chord and for a stronger version of the same reason: it is the
        // route back from a client that has BOTH grabbed the keyboard and taken
        // it out of K_XLATE, which is the one combination the chord above
        // cannot answer because it gates itself on the keyboard mode. Being
        // here, upstream of `broadcast`, is the entire safety argument for
        // enforcing EVIOCGRAB at all: this key is not routed, so it cannot be
        // grabbed.
        //
        // `escape_key` has already asked for the switch back to VT 1; the
        // grabs are this crate's to release.
        if tty_server::vt::escape_key(code, value) {
            release_all_grabs();
            bump(&C_PUSHED, dev_id as usize);
            return;
        }
    }

    // Whether this keystroke belongs to the kernel's line discipline. It does
    // not while a compositor holds the active VT in KD_GRAPHICS, nor while that
    // VT's owner reads scancodes itself (K_RAW/K_MEDIUMRAW/K_OFF) — in either
    // case queueing a copy for the console is what makes a shell replay
    // everything typed into a full-screen client once it exits, which is
    // measurable: 10 keys injected into two evdev readers came back as
    // `aaaaaaaaaa` at the prompt afterwards. Two relaxed atomic loads.
    //
    // SERIAL IS EXEMPT, and that exemption is load-bearing. The serial line is
    // this machine's primary console and is not a VT; on Linux it is a separate
    // tty that the VT keyboard mode has no say over. Here it is unified into
    // evdev — the UART drain manufactures a synthetic EV_KEY with value == 2 on
    // the keyboard node for each byte (arch/*/timer.rs `on_tick`, and the
    // reason `read_input_byte` special-cases that value) — so without this
    // clause the first compositor to take the display would cut off the only
    // way to drive a headless boot.
    let serial_byte = dev_id == DEV_KEYBOARD as u32 && type_ == EV_KEY && value == 2;
    let console_ok = serial_byte || tty_server::vt::console_keyboard_active();

    // Widen the serial exemption from the key to its whole FRAME — see
    // `LAST_KB_SERIAL`. Deliberately NOT folded into `console_ok`: the console
    // tap discards everything that is not a key-down anyway, and queueing SYNs
    // into it under KD_GRAPHICS would leave `has_events` true with nothing
    // `read_input_byte` could ever return, which is the exact confusion
    // `has_key_event` exists to undo.
    let serial_frame = if dev_id == DEV_KEYBOARD as u32 {
        use core::sync::atomic::Ordering::Relaxed;
        if type_ == EV_KEY {
            LAST_KB_SERIAL.store(value == 2, Relaxed);
            serial_byte
        } else {
            type_ == EV_SYN && code == SYN_REPORT && LAST_KB_SERIAL.load(Relaxed)
        }
    } else {
        false
    };

    // Which VT the event belongs to, for the per-client gate in `broadcast`.
    // One relaxed atomic load; safe from this IRQ context for the same reason
    // `chord_key` is.
    //
    // SERIAL IS EXEMPT HERE TOO (`serial_frame`, computed above), for the reason
    // above and one more. The console
    // tap already survives the gate by carrying `vt == 0`, so the exemption is
    // not what keeps a serial boot's own shell alive — but a *userspace* reader
    // of `/dev/input/event0` on a serial-only machine has no other source of
    // keystrokes at all, and pinning it to the VT it happened to start on would
    // make the UART stop reaching it the moment anything took another VT. That
    // is the same reasoning `console_ok` already applies one line up, extended
    // to every queue rather than just the console's; a headless boot must not
    // be able to lose its only input path to a display-arbitration decision.
    //
    // The test is `value == 2`, which a real keyboard's AUTOREPEAT also
    // produces — so an autorepeat reaches a backgrounded queue. That
    // conflation is pre-existing (`console_ok` is built from the same
    // predicate) and the cost of it is one extra repeat event, not a stuck key:
    // the key-down that started the repeat was gated, and xkb ignores a repeat
    // for a key it never saw pressed.
    let active_vt = tty_server::vt::active() as u32;

    let ev = input_event {
        time: timeval {
            tv_sec: (now_us / 1_000_000) as i64,
            tv_usec: (now_us % 1_000_000) as i64,
        },
        type_,
        code,
        value,
    };
    let f = unsafe { arch_interrupt_save() };
    // Broadcast, not enqueue-once: on Linux an event goes to every open of the
    // node and one reader consuming it never removes another's copy.
    let overflowed = STATE.lock().broadcast(dev_id, ev, console_ok, active_vt, serial_frame);
    bump(&C_PUSHED, dev_id as usize);
    if overflowed { bump(&C_DROPPED, dev_id as usize); }
    // A key event is a POLLIN edge for a console reader / evdev poller parked on
    // the poll wait-channel. try_wake (non-blocking) honors IRQ context; the
    // 100 Hz console-read tick is the backstop if RUN_QUEUE is momentarily busy.
    //
    // WHICH events wake is the coalescing decision — see `WAKE_MODE`. The wake
    // stays INSIDE the interrupt-masked region in every mode, where it already
    // was: `try_wake_poll` takes RUN_QUEUE, and this runs in IRQ context.
    match WAKE_MODE {
        WAKE_MODE_SYN => {
            if type_ == EV_SYN { wake_pollers(); }
        }
        WAKE_MODE_BURST => {
            // Defer to the end of the drain. Nothing is lost if a burst somehow
            // ends without a flush: the next event's flush picks the flag up,
            // and the poll-deadline tick is the standing backstop underneath.
            WAKE_PENDING.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        _ => wake_pollers(),
    }
    unsafe { arch_interrupt_restore(f); }
}

/// VT `new_vt` (1-based) has just come on screen — resynchronise every queue
/// pinned to it.
///
/// Called from `tty_server::vt::complete_switch` through the same
/// `#[no_mangle]` seam the framebuffer bridge uses, and for the same reason:
/// `evdev-server` depends on `tty-server` (for `chord_key`), so the tty crate
/// cannot name this one back without a cycle.
///
/// TASK CONTEXT, and it must stay that way — it takes `STATE`, which the input
/// IRQ also takes, so interrupts are masked across the whole update. It does
/// **not** wake pollers: `complete_switch` calls `sched::wake_poll` immediately
/// after, and one wake for the whole switch is enough.
/// It also **drops every `EVIOCGRAB` on every node** — the ungrab-all that
/// item 20's escape hatch is built out of.
///
/// A grab is scoped to the VT that took it and does not survive a switch away
/// from that VT, which is enforced here rather than by comparing `grab_vt`
/// against the live VT on the push path: a grab that went dormant and woke up
/// again would mean "the client I switched away from is still holding the
/// keyboard", and switching away is exactly how a user says they no longer
/// want that. It costs a libseat client nothing, because it already closes and
/// reopens its devices across a switch and re-grabs on the way back in.
#[no_mangle]
pub extern "C" fn evdev_vt_activated(new_vt: u32) {
    if new_vt == 0 { return; }
    let now_us = unsafe { arch_monotonic_ns() } / 1_000;
    let f = unsafe { arch_interrupt_save() };
    let mut st = STATE.lock();
    st.ungrab_all();
    for c in st.clients.iter_mut() {
        if c.in_use && c.vt != 0 && c.vt == new_vt {
            c.force_resync(now_us);
        }
    }
    drop(st);
    unsafe { arch_interrupt_restore(f); }
}

/// Release every `EVIOCGRAB` on every node, from anywhere.
///
/// Two callers, both of them rescues: the Ctrl+Alt+Esc escape hatch on the IRQ
/// push path, and process teardown. IRQ-safe — one lock, no user memory, no
/// `RUN_QUEUE`.
#[no_mangle]
pub extern "C" fn release_all_grabs() -> bool {
    let f = unsafe { arch_interrupt_save() };
    let any = STATE.lock().ungrab_all();
    unsafe { arch_interrupt_restore(f); }
    any
}

/// A process exited — drop any grab it held.
///
/// `VFS_CLOSE` covers an orderly exit and `broadcast` self-heals a grab whose
/// client is gone, but `broadcast` only runs when an event arrives and a
/// grabbed node delivers to nobody, so neither is a guarantee for a process
/// that was killed. This runs from the same task-context teardown that already
/// calls `vt::cleanup_pid`, and it is the one release that does not depend on
/// the dead process having cooperated.
pub fn cleanup_pid(pid: u32) {
    if pid == 0 { return; }
    let f = unsafe { arch_interrupt_save() };
    let mut st = STATE.lock();
    let holders: [(u32, u32); MAX_DEVICES] =
        core::array::from_fn(|d| (st.devs[d].grab_open, st.devs[d].grab_pid));
    for (d, (g_open, g_pid)) in holders.iter().enumerate() {
        if *g_open == 0 && *g_pid == 0 { continue; }
        // The grab is keyed by open, not by pid, so the pid it belongs to is
        // whichever client answers that key — `grab_pid` alone is only
        // authoritative for a pid-keyed queue.
        let owner = st.clients.iter()
            .find(|c| c.dev_id == d as u32 && client_matches(c, *g_open, *g_pid))
            .map(|c| c.pid);
        if owner == Some(pid) || (owner.is_none() && *g_pid == pid) {
            st.devs[d].ungrab();
        }
    }
    drop(st);
    unsafe { arch_interrupt_restore(f); }
}

/// Announce a newly registered queue and, above all, **the VT it was tagged
/// with**.
///
/// This is the one line that names the gate's own failure mode. A device
/// opened while its owner is backgrounded is tagged with the background VT and
/// receives nothing until that VT returns; from userspace that is
/// indistinguishable from a driver that stopped working, and without the tag in
/// the log there is nothing to look at. Registration is rare — one line per
/// `open()` of an input node, a handful per session even with a compositor that
/// reopens its devices on every switch — so this is not gated behind a
/// diagnostic const the way `[EVSTAT]` is: a diagnostic you have to rebuild to
/// enable is not there when the report arrives.
fn log_registration(dev: u32, open_id: u32, pid: u32, vt: u32) {
    fn s(m: &str) { for &b in m.as_bytes() { unsafe { arch_serial_putc(b) } } }
    fn d(mut v: u64) {
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 { break; }
        }
        for &b in &buf[i..] { unsafe { arch_serial_putc(b) } }
    }
    s("[EVDEV] register dev="); d(dev as u64);
    s(" open=");
    if open_id == CONSOLE_OPEN_ID { s("console"); } else { d(open_id as u64); }
    s(" pid="); d(pid as u64);
    s(" vt=");
    if vt == 0 { s("follow"); } else { d(vt as u64); }
    s("\n");
}

pub fn init(owner_pid: u32) -> Option<u32> {
    let port_id = port::create(owner_pid)?;
    {
        let mut st = STATE.lock();
        st.devs[DEV_KEYBOARD].in_use = true; // event0 (keyboard)
        st.devs[DEV_TABLET].in_use = true;   // event1 (virtio-tablet absolute pointer)
        // The console drain's queue, registered for the lifetime of the system
        // on the keyboard node only: `read_input_byte` is the sole caller of
        // pop_event and only ever asks for device 0, and a tap on the tablet
        // would just accumulate motion nobody pops.
        st.find_or_register(DEV_KEYBOARD as u32, CONSOLE_OPEN_ID, 0);
    }
    vfs_server::register_device("/dev/input/event0", port_id, 0, 13, 64);
    vfs_server::register_device("/dev/input/event1", port_id, 1, 13, 65);
    port::register_handler(port_id, handle);
    Some(port_id)
}
