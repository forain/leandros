//! Lane playcost counters, printed as one `# playcost` row of `/proc/rqhold`.
//! `WIDEN_*`: `widen_port_masks` parkers skipped by the park mirrors vs
//! dereferenced. `FWAKE_*`: FUTEX_WAKEs that found no claimable waiter (and so
//! never took RUN_QUEUE) vs ones that did.
use core::sync::atomic::AtomicU64;
pub static WIDEN_SKIP:   AtomicU64 = AtomicU64::new(0);
pub static WIDEN_DEREF:  AtomicU64 = AtomicU64::new(0);
pub static FWAKE_EMPTY:  AtomicU64 = AtomicU64::new(0);
pub static FWAKE_LOCKED: AtomicU64 = AtomicU64::new(0);
/// FUTEX_WAIT returns whose waker already reset the task (no second RUN_QUEUE hold).
pub static FWAIT_TIDY:   AtomicU64 = AtomicU64::new(0);
/// Poll-deadline service passes (`wake_due_poll_deadlines_keyed`), the parked
/// slots they examined, and passes that fell back to the full O(parked) scan.
pub static DL_PASS:     AtomicU64 = AtomicU64::new(0);
pub static DL_SLOTS:    AtomicU64 = AtomicU64::new(0);
pub static DL_FULL:     AtomicU64 = AtomicU64::new(0);
