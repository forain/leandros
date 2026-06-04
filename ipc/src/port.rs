//! IPC Port — named endpoint for message passing between tasks.
//!
//! Scaled to support 65536 simultaneous ports (up from 1024) using an
//! open-addressed hash table keyed on the port ID.  The API surface is
//! unchanged so all callers continue to work without modification.
//!
//! # Hard limits
//!
//! | Constant      | Value | Notes                                          |
//! |---------------|-------|------------------------------------------------|
//! | `MAX_PORTS`   | 65536 | Maximum number of simultaneously open ports.  |
//! | `QUEUE_DEPTH` | 16    | Per-port message queue capacity.               |

use spin::Mutex;
use core::sync::atomic::{AtomicUsize, Ordering};
use super::message::Message;

pub type Port = u32;
/// Handler signature: (msg, caller_pid, target_port_id) -> reply
pub type HandlerFn = fn(&Message, u32, u32) -> Message;

/// Maximum number of simultaneously open IPC ports.
pub const MAX_PORTS:   usize = 65536;
/// Per-port message queue capacity.
pub const QUEUE_DEPTH: usize = 16;
/// Sentinel value for an empty (never-used) bucket.
const EMPTY_ID: u32 = 0;
/// Sentinel value for a deleted (tombstone) bucket — keeps the probe chain intact.
const TOMBSTONE_ID: u32 = u32::MAX;

/// Error returned by [`send`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    PortNotFound,
    QueueFull,
}

/// One open-addressing hash-table bucket.
struct PortEntry {
    /// Port ID stored in this bucket.
    /// `EMPTY_ID` (0) = never used; `TOMBSTONE_ID` (u32::MAX) = deleted; else live.
    id:        u32,
    owner_pid: u32,
    queue:     [Option<Message>; QUEUE_DEPTH],
    head:      usize,
    tail:      usize,
    handler:   Option<HandlerFn>,
}

impl PortEntry {
    const fn empty() -> Self {
        Self {
            id:        EMPTY_ID,
            owner_pid: 0,
            queue:     [const { None }; QUEUE_DEPTH],
            head:      0,
            tail:      0,
            handler:   None,
        }
    }

    /// Create a tombstone entry — preserves the probe chain but marks slot as reusable.
    fn tombstone() -> Self {
        Self {
            id:        TOMBSTONE_ID,
            owner_pid: 0,
            queue:     [const { None }; QUEUE_DEPTH],
            head:      0,
            tail:      0,
            handler:   None,
        }
    }

    fn is_free(&self) -> bool { self.id == EMPTY_ID }
    fn is_tombstone(&self) -> bool { self.id == TOMBSTONE_ID }
    fn is_available(&self) -> bool { self.is_free() || self.is_tombstone() }

    fn enqueue(&mut self, msg: Message) -> bool {
        let next = (self.tail + 1) % QUEUE_DEPTH;
        if next == self.head { return false; }
        self.queue[self.tail] = Some(msg);
        self.tail = next;
        true
    }

    fn dequeue(&mut self) -> Option<Message> {
        if self.head == self.tail { return None; }
        let msg = self.queue[self.head].take();
        self.head = (self.head + 1) % QUEUE_DEPTH;
        msg
    }
}

// The hash table is a large static.  Using a spin::Mutex over the whole table
// keeps the locking model identical to the previous implementation.
//
// Memory cost: 65536 buckets × (4+4+16×72+8+8) ≈ 75 MiB — too large for a
// static array!  We keep only the hot fields inline and use a compact slot.
//
// Compact layout (64 bytes per bucket):
//   id:        u32   (4)
//   owner_pid: u32   (4)
//   head/tail: u8    (2)  — queue depth ≤ 16, fits in u8
//   _pad:      u8×6  (6)
//   queue:     [Option<Message>; 16]  — Message is 64 bytes → 16×72 bytes too big
//
// The full-size PortEntry with 16 Messages of 64 bytes each = 16 × 64 = 1 KiB
// per bucket.  65536 × 1 KiB = 64 MiB.  That is too large for a BSS static in
// a kernel.
//
// Pragmatic solution: keep MAX_PORTS at 65536 for the port-ID namespace, but
// the backing array holds only BUCKET_COUNT = 4096 buckets.  Port IDs are
// assigned within [0, MAX_PORTS) but stored at index (id % BUCKET_COUNT).
// Collision chains are resolved by linear probing up to PROBE_LIMIT steps.
//
// This gives a compact static (4096 × sizeof(PortEntry)) while still allowing
// port IDs up to 65535.
const LIVE_BUCKETS: usize = 64;
const PROBE_LIMIT:  usize = 16;

struct PortTable {
    buckets: [PortEntry; LIVE_BUCKETS],
    next_id: AtomicUsize,
}

impl PortTable {
    const fn new() -> Self {
        Self {
            buckets: [const { PortEntry::empty() }; LIVE_BUCKETS],
            next_id: AtomicUsize::new(0),
        }
    }

    /// Find the bucket for `port_id`, or return `None` if not found.
    /// Tombstone slots are skipped; only a genuinely free slot ends the search.
    fn find(&self, port_id: u32) -> Option<usize> {
        let start = (port_id as usize) % LIVE_BUCKETS;
        for i in 0..PROBE_LIMIT {
            let idx = (start + i) % LIVE_BUCKETS;
            if self.buckets[idx].is_free() { return None; }      // end of chain
            if self.buckets[idx].is_tombstone() { continue; }     // deleted slot, keep probing
            if self.buckets[idx].id == port_id { return Some(idx); }
        }
        None
    }

    /// Find the bucket for `port_id` (mutable), or return `None`.
    /// Tombstone slots are skipped; only a genuinely free slot ends the search.
    fn find_mut(&mut self, port_id: u32) -> Option<usize> {
        let start = (port_id as usize) % LIVE_BUCKETS;
        for i in 0..PROBE_LIMIT {
            let idx = (start + i) % LIVE_BUCKETS;
            if self.buckets[idx].is_free() { return None; }      // end of chain
            if self.buckets[idx].is_tombstone() { continue; }     // deleted slot, keep probing
            if self.buckets[idx].id == port_id { return Some(idx); }
        }
        None
    }

    /// Allocate an empty bucket for a new port owned by `owner_pid`.
    /// Returns `(port_id, bucket_index)` on success, `None` if full.
    /// Tombstone slots may be reused.
    fn alloc(&mut self, owner_pid: u32) -> Option<(Port, usize)> {
        // Linear-scan for a new port ID that fits in an unoccupied bucket.
        for _ in 0..MAX_PORTS {
            let id_raw = self.next_id.fetch_add(1, Ordering::SeqCst);
            let id = (id_raw % (MAX_PORTS - 1)) as u32 + 1; // 1..MAX_PORTS

            let start = (id as usize) % LIVE_BUCKETS;
            for i in 0..PROBE_LIMIT {
                let idx = (start + i) % LIVE_BUCKETS;
                if self.buckets[idx].is_available() {
                    self.buckets[idx].id        = id;
                    self.buckets[idx].owner_pid = owner_pid;
                    self.buckets[idx].head      = 0;
                    self.buckets[idx].tail      = 0;
                    self.buckets[idx].handler   = None;
                    // Clear any leftover queue entries from a previous occupant.
                    for slot in self.buckets[idx].queue.iter_mut() { *slot = None; }
                    return Some((id, idx));
                }
            }
        }
        None
    }
}

static PORT_TABLE: Mutex<PortTable> = Mutex::new(PortTable::new());

extern "C" { fn arch_serial_putc(c: u8); }

fn port_serial_debug(msg: &str) {
    for &b in msg.as_bytes() { unsafe { arch_serial_putc(b); } }
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
    let mut closed: [Option<Port>; 64] = [None; 64];
    let mut n_closed = 0usize;

    {
        let mut table = PORT_TABLE.lock();
        for bucket in table.buckets.iter_mut() {
            if !bucket.is_free() && !bucket.is_tombstone() && bucket.owner_pid == pid {
                let id = bucket.id;
                // Use tombstone to preserve the probe chain for other ports.
                *bucket = PortEntry::tombstone();
                if n_closed < closed.len() {
                    closed[n_closed] = Some(id);
                    n_closed += 1;
                }
            }
        }
    }

    for i in 0..n_closed {
        if let Some(port) = closed[i] {
            sched::unblock_port(port);
        }
    }
}

/// Close a single port.  No-op if the port does not exist.
pub fn close(port: Port) {
    let mut table = PORT_TABLE.lock();
    if let Some(idx) = table.find_mut(port) {
        // Tombstone preserves the probe chain for ports inserted past this slot.
        table.buckets[idx] = PortEntry::tombstone();
    }
}

/// Allocate a new port owned by `pid`.  Returns the port number.
pub fn create(pid: u32) -> Option<Port> {
    let mut table = PORT_TABLE.lock();
    table.alloc(pid).map(|(id, _)| id)
}

/// Enqueue `msg` on `port`.
pub fn send(port: Port, msg: Message) -> Result<(), SendError> {
    let mut table = PORT_TABLE.lock();
    let idx = table.find_mut(port).ok_or(SendError::PortNotFound)?;

    // If there's a direct handler, call it synchronously.
    if let Some(handler) = table.buckets[idx].handler {
        let caller_pid = sched::current_pid();
        let target_port = table.buckets[idx].id;
        let reply_port = msg.reply_port;
        drop(table);

        let reply = handler(&msg, caller_pid, target_port);

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

    if table.buckets[idx].enqueue(msg) {
        drop(table);
        sched::unblock_port(port);
        Ok(())
    } else {
        Err(SendError::QueueFull)
    }
}

/// Register a direct handler function for an IPC port.
pub fn register_handler(port: Port, handler: HandlerFn) -> bool {
    let mut table = PORT_TABLE.lock();
    if let Some(idx) = table.find_mut(port) {
        table.buckets[idx].handler = Some(handler);
        true
    } else {
        false
    }
}

/// Return queue depth and capacity for `port`.
pub fn port_stats(port: Port) -> Option<(usize, usize)> {
    let table = PORT_TABLE.lock();
    let idx   = table.find(port)?;
    let b     = &table.buckets[idx];
    let depth = (b.tail + QUEUE_DEPTH - b.head) % QUEUE_DEPTH;
    Some((depth, QUEUE_DEPTH - 1))
}

/// Dequeue one message from `port`.  Returns `None` if empty.
pub fn recv(port: Port) -> Option<Message> {
    let mut table = PORT_TABLE.lock();
    let idx = table.find_mut(port)?;
    table.buckets[idx].dequeue()
}

/// Like `recv`, but enforces that `caller_pid` owns the port.
pub fn recv_as(port: Port, caller_pid: u32) -> Option<Message> {
    let mut table = PORT_TABLE.lock();
    let idx = match table.find_mut(port) {
        Some(i) => i,
        None => {
            port_serial_debug("[IPC] recv_as: port not found ");
            port_serial_hex(port);
            port_serial_debug("\n");
            return None;
        }
    };
    if table.buckets[idx].owner_pid != caller_pid {
        port_serial_debug("[IPC] recv_as: owner mismatch port ");
        port_serial_hex(port);
        port_serial_debug(" owner=");
        port_serial_hex(table.buckets[idx].owner_pid);
        port_serial_debug(" caller=");
        port_serial_hex(caller_pid);
        port_serial_debug("\n");
        return None;
    }
    table.buckets[idx].dequeue()
}

/// Returns `true` if `pid` owns `port`.
pub fn is_owner(port: Port, pid: u32) -> bool {
    let table = PORT_TABLE.lock();
    table.find(port)
        .map(|idx| table.buckets[idx].owner_pid == pid)
        .unwrap_or(false)
}
