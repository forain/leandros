# lane/zaplog 2026-11-09 - pidns zap timeout is now logged

FIXED. `sched::pidns::zap` gave up silently after its 5 s bounded wait.

- sched has no log facility (no println in the crate). Added `sched::register_log_hook(fn(&[u8]))`
  (same pattern as the other sched->kernel hooks); kernel/src/init.rs registers `crate::stat_line`
  (console, or klog only on rpi5). No new crate dependency, no cycle.
- On timeout `warn_timeout` emits, at most once per 10 s:
  `[pidns] zap timeout: ns <id> init <tgid> still has <n> live task(s), first: <pid...>` (up to 8 pids).
- Test: no real test path (a ns child cannot survive SIGKILL). Proved the log path with a TEMPORARY
  local build forcing the deadline to 0 (`saturating_add(0)`, NOT committed): `/bin/nstest` aarch64
  PASS (fails=0) and dmesg had `[pidns] zap timeout: ns 2 init 2568 still has 1 live task(s), first: 2569`.
- Default regress aarch64 via box farm: see merge report.
