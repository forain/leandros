# spin 0.9.8, LeandrOS patch

Upstream spin 0.9.8 plus one change (lane pools, 2026-10-19):

- `src/held.rs`: a hook (`spin::held::set_hook`) called on every
  `SpinMutex` acquire (`lock`, a successful `try_lock`) and guard drop.
- `src/mutex/spin.rs`: the call sites: `lock`, `try_lock`, the guard drop,
  and `force_unlock` (the release of a forgotten guard; lane ipcregress
  2026-10-24: without it `TrackedGuard::unlocked` leaked one count per call).

The kernel installs the hook in `sched::spinheld` and keeps a per-CPU count of
held `spin::Mutex`es: `might_sleep()` reports a sleep with one held, the
voluntary-switch path (`sched::yield_now`) reports it and carries the count
with the task, and `preempt_check` defers preemption while it is non-zero.
Benches, examples and the criterion dev-dependency were dropped.
