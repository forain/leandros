//! Per-thread loadable-module generation (`kmod_gen`), kept in a per-CPU slot and carried
//! with the task across voluntary switches (same scheme as `spinheld`). 0 = not in module code.
//! Set by the kmod gate around every kernel-to-module call; read by the kmod export layer
//! to attribute registrations to the generation that is running (never to a module-passed id).

use core::sync::atomic::{AtomicU16, Ordering};
use crate::MAX_CPUS;

static GEN: [AtomicU16; MAX_CPUS] = [const { AtomicU16::new(0) }; MAX_CPUS];

#[inline(always)]
fn cpu() -> usize { (unsafe { crate::cpu_id() }).min(MAX_CPUS - 1) }

/// The generation of the module code running on this thread (0 = none).
pub fn get() -> u16 { GEN[cpu()].load(Ordering::Relaxed) }

/// Set the current generation (callers save and restore in stack order).
pub fn set(g: u16) { GEN[cpu()].store(g, Ordering::Relaxed); }

/// Before a context switch: take the outgoing thread's value; the next thread starts at 0.
pub(crate) fn switch_out() -> u16 { GEN[cpu()].swap(0, Ordering::Relaxed) }

/// After resuming: restore this thread's value on the CPU it resumed on.
pub(crate) fn switch_in(g: u16) { GEN[cpu()].store(g, Ordering::Relaxed); }
