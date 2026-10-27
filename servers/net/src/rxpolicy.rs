//! What the net daemon does with a netif's receive ring on one pass, and how
//! long it may keep re-polling without parking. Pure logic (no kernel types),
//! compiled into the host tests in `servers/net/host-tests` via `#[path]`.
//!
//! Background (lane pirxwedge, 2026-10-27): the daemon is a kernel task and
//! runs with IRQs masked except where it parks. A netif that had no stack
//! (Wi-Fi learns its MAC from the firmware, after `init_nic` ran) was skipped
//! on every pass, but its `rx_rearm` still said "frames pending", so the
//! daemon `continue`d forever without parking: on a 1-CPU Pi 5 the machine
//! froze at `wifi0: carrier up` with no timer tick left to report it.

/// One netif's share of a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NifAction {
    /// The stack exists and the interface is up: poll it.
    Poll,
    /// No stack yet, but the driver now knows its MAC: build the stack,
    /// then poll it.
    InitStack,
    /// No stack (no MAC yet) or administratively down: drop what the
    /// driver received, at most [`DISCARD_MAX`] frames per pass, so the
    /// ring never stays "pending" for a stack that will not read it.
    Discard,
}

/// Frames dropped per pass for a netif that cannot take them.
pub const DISCARD_MAX: usize = 256;

/// Passes in a row that `rx_rearm` may restart without parking. Past this
/// the daemon parks briefly (the 1 kHz cadence) so ticks, other tasks and
/// the driver's own thread get the CPU.
pub const REARM_MAX: u32 = 64;

/// Park length after [`REARM_MAX`] back-to-back passes.
pub const REARM_PARK_NS: u64 = 1_000_000;

pub fn nif_action(has_stack: bool, admin_down: bool, has_mac: bool) -> NifAction {
    if admin_down { return NifAction::Discard; }
    if has_stack { return NifAction::Poll; }
    if has_mac { NifAction::InitStack } else { NifAction::Discard }
}

/// Counts passes restarted by `rx_rearm` since the daemon last parked.
#[derive(Debug, Default)]
pub struct RearmGuard { streak: u32 }

impl RearmGuard {
    pub const fn new() -> Self { RearmGuard { streak: 0 } }
    /// `pending`: some netif reported frames after the pass. True = start
    /// the next pass at once; false = go on to the park.
    pub fn again(&mut self, pending: bool) -> bool {
        if !pending { return false; }
        if self.streak >= REARM_MAX { return false; }
        self.streak += 1;
        true
    }
    /// The daemon parked (or is about to): a new streak starts.
    pub fn parked(&mut self) { self.streak = 0; }
    /// True when the last `again(true)` was refused by the cap.
    pub fn capped(&self) -> bool { self.streak >= REARM_MAX }
    pub fn streak(&self) -> u32 { self.streak }
}
