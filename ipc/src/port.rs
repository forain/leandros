//! IPC Port — named endpoint for message passing between tasks.
//!
//! The table is a growable slab: a port ID names a slot index plus that
//! slot's generation, so a lookup is one bounds check and one compare, and an
//! ID that outlived its port (a server answering a caller that already exited)
//! can never match the slot's next occupant until the generation wraps.
//!
//! # Limits
//!
//! | Constant              | Value | Notes                                         |
//! |-----------------------|-------|-----------------------------------------------|
//! | `MAX_LIVE_PORTS`      | 65536 | Ports open at once, system-wide.              |
//! | `QUEUE_DEPTH`         | 16    | Queue slots of a server/user port.            |
//! | `REPLY_QUEUE_DEPTH`   | 4     | Queue slots of a per-thread reply port.       |
//!
//! Until 2026-10-19 this was a static open-addressed hash of 512 buckets,
//! each carrying an inline 16-deep queue (~7.5 KiB a bucket). Every thread
//! that touches a mounted filesystem holds one port as its reply port until it
//! exits, so 512 threads were the ceiling for the whole system: thread 513's
//! first `stat` failed with ENOMEM with gigabytes of RAM free. The slab grows
//! on demand (allocation is always done with the table lock dropped), reply
//! ports get a 4-deep queue (~1.9 KiB), and the live count is bounded only by
//! `MAX_LIVE_PORTS`, which is 16x the task table.

#[cfg(not(feature = "kernel"))]
use spin::Mutex;
use core::sync::atomic::{AtomicUsize, Ordering};
use super::message::Message;

extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

pub type Port = u32;
/// Handler signature: (msg, caller_pid, target_port_id) -> reply
pub type HandlerFn = fn(&Message, u32, u32) -> Message;

/// User-send filter: true when a message built by user code (`sys_send` /
/// `sys_call`) may reach the handler. In-kernel senders are never filtered.
pub type UserSendFilter = fn(&Message) -> bool;

/// Queue slots of a port made by [`create`] (one is kept empty by the ring, so
/// `QUEUE_DEPTH - 1` messages fit).
pub const QUEUE_DEPTH: usize = 16;
/// Queue slots of a port made by [`create_reply`]. A reply port has at most
/// one call outstanding; the spare room absorbs a late answer to a call its
/// thread was interrupted out of.
pub const REPLY_QUEUE_DEPTH: usize = 4;
/// Ports that may be open at once, system-wide. The task table holds 4096
/// threads and each holds at most one reply port, so this is never the limit
/// a thread storm meets; it exists so `SYS_PORT_CREATE` in a loop runs out
/// before kernel memory does. Must stay below `1 << IDX_BITS`.
pub const MAX_LIVE_PORTS: usize = 65536;

/// Port ID layout: `(generation << IDX_BITS) | (slot_index + 1)`.
/// `+ 1` keeps 0 unused; `GEN_MAX` keeps every ID below 0xFFF0_0000, clear of
/// the scheduler's wait-channel sentinels (0xFFFF_FFxx) and `u32::MAX` (= no
/// reply port).
const IDX_BITS: u32 = 20;
const IDX_MASK: u32 = (1 << IDX_BITS) - 1;
const GEN_MAX: u16 = 0xFFE;
const _: () = assert!(MAX_LIVE_PORTS < IDX_MASK as usize);
const _: () = assert!(((GEN_MAX as u32) << IDX_BITS | IDX_MASK) < 0xFFFF_FF00);

/// Free-list terminator.
const NIL: u32 = u32::MAX;

/// Error returned by [`send`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    PortNotFound,
    QueueFull,
}

type Queue = Box<[Option<Message>]>;

/// A live port.
struct PortEntry {
    owner_pid: u32,
    head:      u16,
    tail:      u16,
    handler:   Option<HandlerFn>,
    /// Set by `register_user_filter`: which messages user code may send to
    /// this handler port (see `user_send_allowed`). None = none of them:
    /// handler ports are deny-by-default for user senders.
    user_filter: Option<UserSendFilter>,
    /// Set by `register_kernel_handler`: the handler trusts pointers in the
    /// message (VFS/f2fs buffers), so only in-kernel callers may reach it.
    kernel_only: bool,
    queue:     Queue,
}

impl PortEntry {
    fn depth(&self) -> usize { self.queue.len() }

    fn enqueue(&mut self, msg: Message) -> bool {
        let n = self.depth();
        let next = (self.tail as usize + 1) % n;
        if next == self.head as usize { return false; }
        self.queue[self.tail as usize] = Some(msg);
        self.tail = next as u16;
        true
    }

    fn dequeue(&mut self) -> Option<Message> {
        if self.head == self.tail { return None; }
        let msg = self.queue[self.head as usize].take();
        self.head = ((self.head as usize + 1) % self.depth()) as u16;
        msg
    }
}

struct Slot {
    /// Generation of the current (or next) occupant.
    gen:       u16,
    /// Next free slot when `entry` is `None`.
    next_free: u32,
    entry:     Option<PortEntry>,
}

/// Trace every new high-water mark in live-port count. Committed `false`;
/// flip to `true`, rebuild release, and the boot prints one line per new
/// mark showing how many ports were live and which task pushed it there.
pub const PORT_STATS: bool = false;

struct PortTable {
    slots: Vec<Slot>,
    /// FIFO free list, so a just-closed slot (and so its next ID) is the last
    /// to be reused rather than the first.
    free_head: u32,
    free_tail: u32,
    live: usize,
}

fn make_id(gen: u16, idx: usize) -> Port {
    ((gen as u32) << IDX_BITS) | (idx as u32 + 1)
}

impl PortTable {
    const fn new() -> Self {
        Self { slots: Vec::new(), free_head: NIL, free_tail: NIL, live: 0 }
    }

    fn index_of(&self, port_id: u32) -> Option<usize> {
        let low = port_id & IDX_MASK;
        if low == 0 { return None; }
        let idx = (low - 1) as usize;
        let slot = self.slots.get(idx)?;
        if slot.entry.is_some() && make_id(slot.gen, idx) == port_id { Some(idx) } else { None }
    }

    fn entry(&self, port_id: u32) -> Option<&PortEntry> {
        let idx = self.index_of(port_id)?;
        self.slots[idx].entry.as_ref()
    }

    fn entry_mut(&mut self, port_id: u32) -> Option<&mut PortEntry> {
        let idx = self.index_of(port_id)?;
        self.slots[idx].entry.as_mut()
    }

    fn push_free(&mut self, idx: usize) {
        self.slots[idx].next_free = NIL;
        if self.free_tail == NIL {
            self.free_head = idx as u32;
        } else {
            let t = self.free_tail as usize;
            self.slots[t].next_free = idx as u32;
        }
        self.free_tail = idx as u32;
    }

    fn pop_free(&mut self) -> Option<usize> {
        if self.free_head == NIL { return None; }
        let idx = self.free_head as usize;
        self.free_head = self.slots[idx].next_free;
        if self.free_head == NIL { self.free_tail = NIL; }
        Some(idx)
    }

    /// Install `entry` in a free slot without allocating. `Err(entry)` hands
    /// it back when the slab has to grow first.
    fn try_install(&mut self, entry: PortEntry) -> Result<Port, PortEntry> {
        let idx = match self.pop_free() {
            Some(i) => i,
            None if self.slots.len() < self.slots.capacity() => {
                self.slots.push(Slot { gen: 0, next_free: NIL, entry: None });
                self.slots.len() - 1
            }
            None => return Err(entry),
        };
        self.slots[idx].entry = Some(entry);
        self.live += 1;
        Ok(make_id(self.slots[idx].gen, idx))
    }

    /// Close the port in slot `idx` and return its queue, which the caller
    /// frees after dropping the lock.
    fn remove(&mut self, idx: usize) -> Option<Queue> {
        let e = self.slots[idx].entry.take()?;
        let g = self.slots[idx].gen;
        self.slots[idx].gen = if g >= GEN_MAX { 0 } else { g + 1 };
        self.push_free(idx);
        self.live -= 1;
        Some(e.queue)
    }
}

/// Report an exhausted port table once per boot.
///
/// Every caller of `create` turns `None` into ENOMEM -- `servers::vfs::call_port`
/// does it for every operation on a mounted filesystem -- so userspace sees
/// errno 12 and nothing else. Once is enough: an ungated print would be one
/// line per failed call on a console that costs ~0.19 s per line.
fn report_table_full(live: usize, why: &str) {
    use core::sync::atomic::AtomicBool;
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.swap(true, Ordering::Relaxed) { return; }
    port_serial_debug("\n[IPC] port::create failed: ");
    port_serial_debug(why);
    port_serial_debug(", ");
    port_serial_dec(live);
    port_serial_debug("/");
    port_serial_dec(MAX_LIVE_PORTS);
    port_serial_debug(" ports live -- every caller turns this into ENOMEM\n");
}

/// Trace a new live-count high-water mark. Gated on `PORT_STATS`.
fn note_high_water(live: usize, owner_pid: u32) {
    if !PORT_STATS { return; }
    static HIGH: AtomicUsize = AtomicUsize::new(0);
    if HIGH.fetch_max(live, Ordering::Relaxed) >= live { return; }
    port_serial_debug("[IPC] ports live=");
    port_serial_dec(live);
    port_serial_debug(" new high water, pid=");
    port_serial_dec(owner_pid as usize);
    port_serial_debug("\n");
}

#[cfg(feature = "kernel")]
static PORT_TABLE: sched::lockwatch::TrackedMutex<PortTable> =
    sched::lockwatch::TrackedMutex::new(sched::lockwatch::L_PORT_TABLE, PortTable::new());
#[cfg(not(feature = "kernel"))]
static PORT_TABLE: Mutex<PortTable> = Mutex::new(PortTable::new());

extern "C" { fn arch_serial_putc(c: u8); }

fn port_serial_debug(msg: &str) {
    for &b in msg.as_bytes() { unsafe { arch_serial_putc(b); } }
}

fn port_serial_dec(mut v: usize) {
    if v == 0 { unsafe { arch_serial_putc(b'0') }; return; }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 { i -= 1; buf[i] = b'0' + (v % 10) as u8; v /= 10; }
    for &b in &buf[i..] { unsafe { arch_serial_putc(b) }; }
}

fn port_serial_hex(v: u32) {
    port_serial_debug("0x");
    for i in (0..8).rev() {
        let n = (v >> (i * 4)) & 0xF;
        let c = if n < 10 { b'0' + n as u8 } else { b'A' + n as u8 - 10 };
        unsafe { arch_serial_putc(c); }
    }
}

pub fn init() {}

/// Close all ports owned by `pid`.
pub fn release_by_owner(pid: u32) {
    // The batch is a stack buffer, so it is bounded; the outer loop is what
    // keeps that bound from becoming a silent cap. Closing happens under the
    // table lock; waking and freeing the queues happen outside it.
    const BATCH: usize = 64;
    loop {
        let mut closed: [Option<(Port, Queue)>; BATCH] = [const { None }; BATCH];
        let mut n_closed = 0usize;
        {
            let mut table = PORT_TABLE.lock();
            if table.live == 0 { return; }
            let mut idx = 0;
            while idx < table.slots.len() && n_closed < BATCH {
                let owned = table.slots[idx].entry.as_ref().map_or(false, |e| e.owner_pid == pid);
                if owned {
                    let id = make_id(table.slots[idx].gen, idx);
                    if let Some(q) = table.remove(idx) {
                        closed[n_closed] = Some((id, q));
                        n_closed += 1;
                    }
                }
                idx += 1;
            }
        }
        for slot in closed.iter_mut().take(n_closed) {
            if let Some((port, q)) = slot.take() {
                sched::unblock_port(port);
                drop(q);
            }
        }
        if n_closed < BATCH { return; }
    }
}

/// Close a single port.  No-op if the port does not exist.
pub fn close(port: Port) {
    let q = {
        let mut table = PORT_TABLE.lock();
        match table.index_of(port) {
            Some(idx) => table.remove(idx),
            None => None,
        }
    };
    drop(q);
}

fn alloc_queue(depth: usize) -> Option<Queue> {
    let mut v: Vec<Option<Message>> = Vec::new();
    v.try_reserve_exact(depth).ok()?;
    for _ in 0..depth { v.push(None); }
    Some(v.into_boxed_slice())
}

fn create_with_depth(pid: u32, depth: usize) -> Option<Port> {
    let queue = match alloc_queue(depth) {
        Some(q) => q,
        None => { report_table_full(PORT_TABLE.lock().live, "out of memory for the queue"); return None; }
    };
    let mut entry = PortEntry { owner_pid: pid, head: 0, tail: 0, handler: None, user_filter: None, kernel_only: false, queue };
    // A grown slab is allocated with the lock dropped (the allocator is never
    // entered under PORT_TABLE), then swapped in if nobody grew it meanwhile.
    let mut spare: Option<Vec<Slot>> = None;
    loop {
        let mut table = PORT_TABLE.lock();
        if table.live >= MAX_LIVE_PORTS {
            let live = table.live;
            drop(table);
            report_table_full(live, "MAX_LIVE_PORTS reached");
            return None;
        }
        match table.try_install(entry) {
            Ok(id) => {
                let live = table.live;
                drop(table);
                note_high_water(live, pid);
                return Some(id);
            }
            Err(e) => entry = e,
        }
        let len = table.slots.len();
        if let Some(mut v) = spare.take() {
            if v.capacity() > len {
                v.extend(table.slots.drain(..));
                let old = core::mem::replace(&mut table.slots, v);
                drop(table);
                drop(old);
                continue;
            }
            // Someone else grew the slab past it meanwhile: free the stale
            // spare outside the lock and size a new one.
            drop(table);
            drop(v);
            continue;
        }
        let want = (len * 2).max(256).min(MAX_LIVE_PORTS);
        drop(table);
        let mut v: Vec<Slot> = Vec::new();
        if v.try_reserve_exact(want).is_err() {
            report_table_full(len, "out of memory growing the slab");
            return None;
        }
        spare = Some(v);
    }
}

/// Allocate a new port owned by `pid`.  Returns the port number.
pub fn create(pid: u32) -> Option<Port> {
    create_with_depth(pid, QUEUE_DEPTH)
}

/// Allocate a per-thread reply port (a shallower queue: one call at a time).
pub fn create_reply(pid: u32) -> Option<Port> {
    create_with_depth(pid, REPLY_QUEUE_DEPTH)
}

/// Live ports and the system-wide ceiling, so a caller can report the
/// pressure rather than only its failure.
pub fn census() -> (usize, usize) {
    (PORT_TABLE.lock().live, MAX_LIVE_PORTS)
}

/// Enqueue `msg` on `port`.
pub fn send(port: Port, msg: Message) -> Result<(), SendError> {
    let mut table = PORT_TABLE.lock();
    let e = table.entry_mut(port).ok_or(SendError::PortNotFound)?;

    // If there's a direct handler, call it synchronously.
    if let Some(handler) = e.handler {
        let caller_pid = sched::current_pid();
        let reply_port = msg.reply_port;
        drop(table);

        let reply = handler(&msg, caller_pid, port);

        if reply_port != u32::MAX {
            match send(reply_port, reply) {
                Ok(()) => {}
                Err(e) => {
                    port_serial_debug("[IPC] ERROR: reply delivery failed for port ");
                    port_serial_hex(reply_port);
                    port_serial_debug(if e == SendError::PortNotFound { " (PortNotFound)\n" } else { " (QueueFull)\n" });
                }
            }
        }
        return Ok(());
    }

    if e.enqueue(msg) {
        let owner = e.owner_pid;
        drop(table);
        // Only the owner ever receives on a port (`recv_as` refuses anyone
        // else), so when the owner is the task running this very send — the
        // synchronous case: a kernel-context server handler replying to its
        // caller's reply port — nobody can be parked on it, and the wake
        // would be a full RUN_QUEUE scan that finds nothing. That scan was
        // the single largest RUN_QUEUE holder on an idle desktop (≈170 k
        // acquisitions/s, ~400 ns each; see lockwatch's `[RQPROF]`).
        if owner != sched::current_pid() {
            sched::unblock_port(port);
        }
        Ok(())
    } else {
        Err(SendError::QueueFull)
    }
}

/// Register a direct handler function for an IPC port.
///
/// User code may not send to the port (`sys_send`/`sys_call` return EPERM)
/// until `register_user_filter` names the messages it may send: a handler
/// cannot tell kernel-filled identity (open_id, a pid slot, buffer pointers
/// the kernel range-checked) from the same message built by a user process,
/// so a port that nobody audited fails closed. In-kernel `send` (VFS
/// forwards, replies) is never filtered.
pub fn register_handler(port: Port, handler: HandlerFn) -> bool {
    let mut table = PORT_TABLE.lock();
    match table.entry_mut(port) {
        Some(e) => { e.handler = Some(handler); true }
        None => false,
    }
}

/// Return queue depth and capacity for `port`.
pub fn port_stats(port: Port) -> Option<(usize, usize)> {
    let table = PORT_TABLE.lock();
    let b = table.entry(port)?;
    let n = b.depth();
    let depth = (b.tail as usize + n - b.head as usize) % n;
    Some((depth, n - 1))
}

/// Dequeue one message from `port`.  Returns `None` if empty.
pub fn recv(port: Port) -> Option<Message> {
    let mut table = PORT_TABLE.lock();
    table.entry_mut(port)?.dequeue()
}

/// Like `recv`, but enforces that `caller_pid` owns the port.
pub fn recv_as(port: Port, caller_pid: u32) -> Option<Message> {
    let mut table = PORT_TABLE.lock();
    let e = match table.entry_mut(port) {
        Some(e) => e,
        None => {
            port_serial_debug("[IPC] recv_as: port not found ");
            port_serial_hex(port);
            port_serial_debug("\n");
            return None;
        }
    };
    if e.owner_pid != caller_pid {
        let owner = e.owner_pid;
        drop(table);
        port_serial_debug("[IPC] recv_as: owner mismatch port ");
        port_serial_hex(port);
        port_serial_debug(" owner=");
        port_serial_hex(owner);
        port_serial_debug(" caller=");
        port_serial_hex(caller_pid);
        port_serial_debug("\n");
        return None;
    }
    e.dequeue()
}

/// Returns `true` if `pid` owns `port`.
pub fn is_owner(port: Port, pid: u32) -> bool {
    PORT_TABLE.lock().entry(port).map_or(false, |e| e.owner_pid == pid)
}

/// What a sender's permission check needs to know about `port`: `None` if it
/// does not exist, else `Some((has_handler, owner_pid))`. A handler port is a
/// kernel server endpoint; any other port is a queue only its owner reads.
/// Register a handler that only kernel code may call: user `sys_send` /
/// `sys_call` to it is EPERM. For servers whose messages carry raw buffer
/// pointers that the kernel's syscall entries range-check before sending —
/// a user process sending the message itself would bypass that check and
/// could name a kernel address.
pub fn register_kernel_handler(port: Port, handler: HandlerFn) -> bool {
    if !register_handler(port, handler) { return false; }
    if let Some(e) = PORT_TABLE.lock().entry_mut(port) { e.kernel_only = true; }
    true
}

/// True when user code may not send to `port` (see `register_kernel_handler`).
pub fn is_kernel_only(port: Port) -> bool {
    PORT_TABLE.lock().entry(port).map_or(false, |e| e.kernel_only)
}

pub fn send_target(port: Port) -> Option<(bool, u32)> {
    PORT_TABLE.lock().entry(port).map(|e| (e.handler.is_some(), e.owner_pid))
}

/// Restrict which messages user code may send to the handler port `port`.
///
/// A server reached through VFS forwards gets kernel-filled identity in the
/// message (the open's `open_id`, the caller pid): the handler cannot tell
/// such a message from one a user process built itself with `sys_call`, so a
/// port that also takes a user protocol names the tags user code may use and
/// the syscall entries refuse the rest (EPERM) before the handler runs.
pub fn register_user_filter(port: Port, filter: UserSendFilter) -> bool {
    match PORT_TABLE.lock().entry_mut(port) {
        Some(e) if e.handler.is_some() => { e.user_filter = Some(filter); true }
        _ => false,
    }
}

/// False when user code may not send `msg` to `port`: a handler port accepts
/// only what its `register_user_filter` filter allows (none without one, and
/// never for a `register_kernel_handler` port). Queue ports (no handler) are
/// governed by the owner credential check alone.
pub fn user_send_allowed(port: Port, msg: &Message) -> bool {
    let (has_handler, kernel_only, f) = match PORT_TABLE.lock().entry(port) {
        Some(e) => (e.handler.is_some(), e.kernel_only, e.user_filter),
        None => return true,
    };
    if !has_handler { return true; }
    if kernel_only { return false; }
    f.map_or(false, |f| f(msg))
}
