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
/// `wait4` run-queue scans, the slots they dereferenced, and filtered scans
/// that found no child and re-ran unfiltered.
pub static W4_CALLS:    AtomicU64 = AtomicU64::new(0);
pub static W4_SLOTS:    AtomicU64 = AtomicU64::new(0);
pub static W4_FALLBACK: AtomicU64 = AtomicU64::new(0);
/// Lane playcost4: keyed poll-channel wakes (`unblock_port_keyed`), the
/// slots they visited and dereferenced, and those that scanned without the
/// keyed index (`sched.wakeidx` != 1).
pub static KW_CALLS:    AtomicU64 = AtomicU64::new(0);
pub static KW_SLOTS:    AtomicU64 = AtomicU64::new(0);
pub static KW_DEREF:    AtomicU64 = AtomicU64::new(0);
pub static KW_FULL:     AtomicU64 = AtomicU64::new(0);
/// `widen_port_masks` calls, the parked slots they visited, and unindexed ones.
pub static WIDEN_CALLS: AtomicU64 = AtomicU64::new(0);
pub static WIDEN_SLOTS: AtomicU64 = AtomicU64::new(0);
pub static WIDEN_FULL:  AtomicU64 = AtomicU64::new(0);
